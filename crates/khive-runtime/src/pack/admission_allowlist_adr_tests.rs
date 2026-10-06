//! ADR-103 Amendment 3 lists the admission-degrade-safe set by owning pack and
//! states its size. Both are copies of `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`;
//! this test fails when either copy differs from the constant.

use std::collections::BTreeSet;
use std::path::Path;

use super::VerbRegistry;

const ADR_PATH: &str = "../../docs/adr/ADR-103-resource-attribution-model.md";
const SECTION: &str = "## Amendment 3 ";
const COUNT_LEAD: &str = "at this revision the constant holds ";

/// The `(pack, verb)` pairs of the Amendment 3 bullet list, in document order,
/// and the entry count its lead-in states.
fn documented_allowlist(adr: &str) -> (Vec<(String, String)>, usize) {
    let start = adr
        .find(SECTION)
        .expect("ADR-103 has an Amendment 3 section");
    let section = &adr[start..];
    let section = match section[SECTION.len()..].find("\n## ") {
        Some(end) => &section[..SECTION.len() + end],
        None => section,
    };

    let count_at = section
        .find(COUNT_LEAD)
        .expect("Amendment 3 states the constant's entry count")
        + COUNT_LEAD.len();
    let digits: String = section[count_at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let count = digits
        .parse()
        .expect("the stated count is a decimal number");

    // The list is the bullet block after the paragraph that states the count.
    // It may hold blank lines between bullets; any other line ends it.
    let after = &section[count_at..];
    let list_at = after
        .find("\n\n")
        .expect("a blank line ends the paragraph that states the count")
        + 2;
    let lines: Vec<&str> = after[list_at..].lines().collect();
    let mut pairs = Vec::new();
    let mut pack: Option<String> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() {
            match lines[index..].iter().find(|line| !line.trim().is_empty()) {
                Some(next) if next.starts_with("- ") => continue,
                _ => break,
            }
        }
        let entries = if let Some(bullet) = line.strip_prefix("- ") {
            let (name, rest) = bullet
                .split_once(": ")
                .expect("each bullet names its pack before a colon");
            pack = Some(name.to_owned());
            rest
        } else if line.starts_with("  ") && pack.is_some() {
            line
        } else {
            break;
        };
        let pack = pack.as_ref().expect("an entry line follows a bullet");
        assert_eq!(
            entries.matches('`').count() % 2,
            0,
            "unbalanced backtick in the list: {line:?}"
        );
        for (position, part) in entries.split('`').enumerate() {
            if position % 2 == 1 {
                pairs.push((pack.clone(), part.to_owned()));
            } else {
                assert!(
                    part.chars().all(|c| matches!(c, ',' | ';' | '.' | ' ')),
                    "every list entry is a backticked verb: {line:?}"
                );
            }
        }
    }
    (pairs, count)
}

#[test]
fn adr_103_amendment_3_lists_exactly_the_admission_degrade_safe_constant() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(ADR_PATH);
    let adr = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let (documented, stated_count) = documented_allowlist(&adr);

    let code: BTreeSet<(String, String)> = VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS
        .iter()
        .map(|&(pack, verb)| (pack.to_owned(), verb.to_owned()))
        .collect();
    let listed: BTreeSet<(String, String)> = documented.iter().cloned().collect();
    assert_eq!(
        listed.len(),
        documented.len(),
        "ADR-103 Amendment 3 lists a (pack, verb) pair twice"
    );
    assert_eq!(
        listed,
        code,
        "ADR-103 Amendment 3 and ADMISSION_DEGRADE_SAFE_VERBS differ; only in the ADR: {:?}; \
         only in the code: {:?}",
        listed.difference(&code).collect::<Vec<_>>(),
        code.difference(&listed).collect::<Vec<_>>()
    );
    assert_eq!(
        stated_count,
        VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS.len(),
        "ADR-103 Amendment 3 states a count other than the constant's length"
    );
}
