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
}
