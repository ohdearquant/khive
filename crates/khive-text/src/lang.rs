//! Script/alphabet identification for per-language routing decisions.

use std::collections::HashMap;

/// Return whether `c` is in a supported CJK, Hiragana, Katakana, or Hangul block.
///
/// See `crates/khive-text/docs/api/identifier-and-script.md`.
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

/// Return whether strictly more than 15% of `text` characters are CJK.
pub fn contains_cjk(text: &str) -> bool {
    let (cjk_count, char_count) = cjk_counts(text);
    if char_count == 0 {
        return false;
    }
    (cjk_count as f64 / char_count as f64) > 0.15
}

/// The 15% classifier using `f32` arithmetic for existing memory routing behavior.
/// Near the boundary it can differ from [`contains_cjk`], which uses `f64`.
pub fn contains_cjk_f32(text: &str) -> bool {
    let (cjk_count, char_count) = cjk_counts(text);
    if char_count == 0 {
        return false;
    }
    (cjk_count as f32 / char_count as f32) > 0.15
}

fn cjk_counts(text: &str) -> (usize, usize) {
    text.chars().fold((0, 0), |(cjk, total), ch| {
        (cjk + usize::from(is_cjk_char(ch)), total + 1)
    })
}

/// Character-count profile containing CJK and ASCII-letter fractions.
///
/// See `crates/khive-text/docs/api/identifier-and-script.md`.
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
        let (cjk, n) = cjk_counts(text);
        if n == 0 {
            return Self {
                cjk_fraction: 0.0,
                latin_fraction: 0.0,
                char_count: 0,
            };
        }
        let latin = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
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
///
/// See `crates/khive-text/docs/api/identifier-and-script.md`.
pub fn is_meaningful_text_query(query: &str) -> bool {
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

/// Deprecated name for [`is_meaningful_text_query`].
#[deprecated(note = "use is_meaningful_text_query")]
pub fn is_meaningful_query(query: &str) -> bool {
    is_meaningful_text_query(query)
}

#[cfg(test)]
#[path = "lang/tests.rs"]
mod tests;
