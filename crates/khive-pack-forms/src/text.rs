//! Wording comparison: case and punctuation folded, words kept in order.

use std::collections::BTreeSet;

/// Lowercase alphanumeric words joined by single spaces.
pub(crate) fn normalize(text: &str) -> String {
    let mut out = String::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

/// The folded words of `text`, as a set.
pub(crate) fn words(text: &str) -> BTreeSet<String> {
    normalize(text)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Shared words over all words of both sides; 0 when both are empty.
pub(crate) fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// `normalize` after dropping apostrophes, so a possessive folds into its
/// word: "Recipient's" and "Recipients" both give "recipients".
pub(crate) fn fold(text: &str) -> String {
    let joined: String = text
        .chars()
        .filter(|c| !matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{02BC}'))
        .collect();
    normalize(&joined)
}

/// The text after a leading line marker such as "5", "1b", "4.", "(c)",
/// "a)" or "(iv)", when a marker leads it and words follow it.
pub(crate) fn without_marker(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let end = trimmed.find(char::is_whitespace)?;
    let (token, rest) = trimmed.split_at(end);
    let rest = rest.trim_start();
    if is_marker(token) && rest.chars().any(char::is_alphanumeric) {
        Some(rest)
    } else {
        None
    }
}

/// A line number with an optional letter ("5", "1b", "12.", "(3)"), or a
/// single letter or small roman numeral closed by ")", "." or ":" ("(c)",
/// "a.", "iv)").
fn is_marker(token: &str) -> bool {
    let opened = token.starts_with('(');
    let inner = token.strip_prefix('(').unwrap_or(token);
    let (core, close) = match inner.char_indices().last() {
        Some((at, c)) if matches!(c, ')' | '.' | ':') => (&inner[..at], Some(c)),
        _ => (inner, None),
    };
    if core.is_empty() || (opened && close != Some(')')) {
        return false;
    }
    let digits = core.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let tail = &core[digits..];
        return digits <= 3
            && (tail.is_empty()
                || (tail.len() == 1 && tail.chars().all(|c| c.is_ascii_alphabetic())));
    }
    let letter = core.len() == 1 && core.chars().all(|c| c.is_ascii_alphabetic());
    let roman = core.len() <= 4
        && core
            .chars()
            .all(|c| matches!(c.to_ascii_lowercase(), 'i' | 'v' | 'x'));
    close.is_some() && (letter || roman)
}

/// The folds a printed label is matched under: the whole label, and the label
/// without its leading line marker when it has one.
pub(crate) fn label_folds(label: &str, fold_with: fn(&str) -> String) -> Vec<String> {
    let mut out = vec![fold_with(label)];
    if let Some(rest) = without_marker(label) {
        let rest = fold_with(rest);
        if !out.contains(&rest) {
            out.push(rest);
        }
    }
    out
}

/// Words that carry no meaning of their own in a field's wording.
const STOP_WORDS: [&str; 18] = [
    "a", "an", "and", "as", "at", "by", "for", "from", "in", "is", "it", "its", "of", "on", "or",
    "the", "to", "with",
];

/// Endings stripped from a word, longest first, each with what replaces it.
const SUFFIXES: [(&str, &str); 10] = [
    ("ations", ""),
    ("ation", ""),
    ("atures", ""),
    ("ature", ""),
    ("ings", ""),
    ("ing", ""),
    ("ies", "y"),
    ("ied", "y"),
    ("ed", ""),
    ("s", ""),
];

/// A plain stem: one common ending stripped when at least three letters stay,
/// then a final "e". "signed", "signature" and "signing" all give "sign";
/// "withholding" gives "withhold"; "dates" and "dated" give "dat", as "date"
/// does. Words with a digit or a letter outside a-z are kept whole.
pub(crate) fn stem(word: &str) -> String {
    if !word.chars().all(|c| c.is_ascii_lowercase()) {
        return word.to_owned();
    }
    let mut out = word.to_owned();
    if let Some((suffix, with)) = SUFFIXES.iter().find(|(s, _)| word.ends_with(s)) {
        let base = &word[..word.len() - suffix.len()];
        let keeps_s =
            *suffix == "s" && (base.ends_with('s') || base.ends_with('u') || base.ends_with('i'));
        if !keeps_s && base.len() + with.len() >= 3 {
            out = format!("{base}{with}");
        }
    }
    if out.len() > 3 && out.ends_with('e') {
        out.pop();
    }
    out
}

/// The stems of the words of `text` that carry meaning.
pub(crate) fn stems(text: &str) -> BTreeSet<String> {
    fold(text)
        .split(' ')
        .filter(|w| !w.is_empty() && !STOP_WORDS.contains(w))
        .map(stem)
        .collect()
}

/// The initials of a wording: the first letter of every word, and of every
/// word that carries meaning, each kept when it has two letters or more.
pub(crate) fn initials(text: &str) -> Vec<String> {
    let folded = fold(text);
    let all: Vec<&str> = folded.split(' ').filter(|w| !w.is_empty()).collect();
    let meaning: Vec<&str> = all
        .iter()
        .copied()
        .filter(|w| !STOP_WORDS.contains(w))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for list in [all, meaning] {
        let letters: String = list.iter().filter_map(|w| w.chars().next()).collect();
        if letters.chars().count() >= 2 && !out.contains(&letters) {
            out.push(letters);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_spacing_and_punctuation_fold_away() {
        assert_eq!(
            normalize("  Recipient  Account-Number: "),
            "recipient account number"
        );
        assert_eq!(normalize("Fee paid by"), normalize("fee PAID by."));
        assert_eq!(normalize("--"), "");
    }

    #[test]
    fn overlap_counts_shared_words() {
        let a = words("recipient account number");
        let b = words("sender account number");
        assert!((overlap(&a, &b) - 0.5).abs() < 1e-9);
        assert_eq!(overlap(&words(""), &words("")), 0.0);
    }

    #[test]
    fn a_possessive_folds_into_its_word() {
        assert_eq!(fold("Recipient's account no."), "recipients account no");
        assert_eq!(fold("Recipient\u{2019}s"), fold("RECIPIENTS"));
        assert_eq!(
            normalize("Recipient's"),
            "recipient s",
            "normalize keeps the split"
        );
    }

    #[test]
    fn a_leading_line_marker_is_dropped_only_before_words() {
        for (label, rest) in [
            ("5 Payer's name", "Payer's name"),
            ("1b Qualified dividends", "Qualified dividends"),
            ("(c) Date", "Date"),
            ("(a) Date", "Date"),
            ("a) Date", "Date"),
            ("4. Amount", "Amount"),
            ("(12) Total", "Total"),
            ("(iv) Signature", "Signature"),
        ] {
            assert_eq!(without_marker(label), Some(rest), "{label}");
        }
        for label in [
            "Date",
            "5",
            "(c)",
            "a Date",
            "1099 Form",
            "Box 5 total",
            "(c Date",
            "12345 Main",
            "5 --",
        ] {
            assert_eq!(without_marker(label), None, "{label}");
        }
        assert_eq!(label_folds("(a) Date", normalize), ["a date", "date"]);
        assert_eq!(label_folds("Date", normalize), ["date"]);
    }

    #[test]
    fn stems_join_the_forms_of_a_word() {
        for (a, b) in [
            ("signed", "signature"),
            ("signing", "sign"),
            ("withholding", "withhold"),
            ("dates", "date"),
            ("dated", "date"),
            ("recipients", "recipient"),
            ("companies", "company"),
            ("routing", "route"),
            ("information", "inform"),
        ] {
            assert_eq!(stem(a), stem(b), "{a} and {b}");
        }
        for kept in ["address", "status", "no", "bring", "1099", "résumé"] {
            assert_eq!(stem(kept), kept, "{kept}");
        }
        assert_ne!(stem("number"), stem("account"));
    }

    #[test]
    fn stems_drop_words_without_meaning() {
        assert_eq!(stems("number of the account"), stems("Account number"));
        assert_eq!(stems("date signed"), stems("signature date"));
        assert!(stems("of the").is_empty());
    }

    #[test]
    fn initials_come_from_every_word_and_from_the_meaningful_ones() {
        assert_eq!(initials("recipient account number"), ["ran"]);
        assert_eq!(initials("Social Security Number"), ["ssn"]);
        assert_eq!(initials("date of birth"), ["dob", "db"]);
        assert!(initials("name").is_empty(), "one letter is not initials");
    }
}
