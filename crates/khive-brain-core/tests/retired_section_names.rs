//! Persisted section posterior maps written before `references` and `other` were
//! retired still load; any other unknown section key remains a load error.

use khive_brain_core::{BetaPosterior, SectionPosteriorSnapshot, SectionType};
use serde_json::{json, Map, Value};

fn alpha_of(index: usize) -> f64 {
    2.0 + index as f64
}

fn beta_of(index: usize) -> f64 {
    3.0 + index as f64
}

/// A section map holding the eight current keys, each with its own values,
/// plus every key in `extra` carrying distinct sentinel values.
fn section_map(extra: &[&str]) -> Value {
    let mut map = Map::new();
    for (index, section) in SectionType::ALL.iter().enumerate() {
        map.insert(
            section.as_str().to_string(),
            json!({"alpha": alpha_of(index), "beta": beta_of(index)}),
        );
    }
    for key in extra {
        map.insert((*key).to_string(), json!({"alpha": 9.0, "beta": 9.0}));
    }
    Value::Object(map)
}

fn snapshot_text(posterior_extra: &[&str], prior_extra: &[&str]) -> String {
    json!({
        "posteriors": section_map(posterior_extra),
        "priors": section_map(prior_extra),
        "total_events": 7,
        "exploration_epoch": 11,
    })
    .to_string()
}

#[test]
fn snapshot_with_retired_keys_loads_with_the_eight_current_entries_unchanged() {
    let text = snapshot_text(&["references", "other"], &["references", "other"]);
    let snapshot: SectionPosteriorSnapshot =
        serde_json::from_str(&text).expect("retired keys must not fail the load");

    assert_eq!(snapshot.posteriors.len(), 8);
    assert_eq!(snapshot.priors.len(), 8);
    for (index, section) in SectionType::ALL.iter().enumerate() {
        let expected = BetaPosterior::new(alpha_of(index), beta_of(index));
        assert_eq!(snapshot.posteriors[section], expected, "{section}");
        assert_eq!(snapshot.priors[section], expected, "{section}");
    }
    assert_eq!(snapshot.total_events, 7);
    assert_eq!(snapshot.exploration_epoch, 11);
}

#[test]
fn snapshot_with_one_retired_key_in_only_one_map_also_loads() {
    let text = snapshot_text(&["other"], &[]);
    let snapshot: SectionPosteriorSnapshot =
        serde_json::from_str(&text).expect("a retired key in posteriors alone must load");
    assert_eq!(snapshot.posteriors.len(), 8);
    assert_eq!(snapshot.priors.len(), 8);
}

#[test]
fn snapshot_with_another_unknown_key_fails_to_load() {
    let in_both = snapshot_text(
        &["references", "other", "glossary"],
        &["references", "other"],
    );
    let error = serde_json::from_str::<SectionPosteriorSnapshot>(&in_both)
        .expect_err("an unknown key in posteriors must stay a load error");
    assert!(error.to_string().contains("glossary"), "{error}");

    let in_priors = snapshot_text(&[], &["glossary"]);
    let error = serde_json::from_str::<SectionPosteriorSnapshot>(&in_priors)
        .expect_err("an unknown key in priors must stay a load error");
    assert!(error.to_string().contains("glossary"), "{error}");
}

#[test]
fn retired_names_are_recognised_but_are_not_section_types() {
    assert_eq!(SectionType::ALL.len(), 8);
    assert_eq!(SectionType::NAMES.len(), 8);
    for name in ["references", "other"] {
        assert!(SectionType::RETIRED_NAMES.contains(&name));
        assert!(SectionType::is_retired_name(name));
        assert!(!SectionType::NAMES.contains(&name));
        assert!(name.parse::<SectionType>().is_err());
        assert!(SectionType::from_str_loose(name).is_none());
    }
    for name in SectionType::NAMES {
        assert!(!SectionType::is_retired_name(name));
    }
    assert!(!SectionType::is_retired_name("glossary"));
    for alias in [
        "reference",
        "bibliography",
        "related",
        "see_also",
        "further_reading",
        "citations",
        "links",
        "misc",
        "miscellaneous",
        "notes",
        "appendix",
    ] {
        assert!(SectionType::from_str_loose(alias).is_none(), "{alias}");
    }
}

#[test]
fn every_retired_spelling_is_recognised_after_normalization_and_never_parses() {
    for (spelling, retired) in SectionType::RETIRED_SPELLINGS {
        assert!(SectionType::RETIRED_NAMES.contains(retired), "{spelling}");
        assert_eq!(SectionType::normalize_name(spelling), *spelling);
        assert_eq!(SectionType::retired_type_of(spelling), Some(*retired));
        assert!(
            SectionType::from_str_loose(spelling).is_none(),
            "{spelling}"
        );
        let upper = spelling.to_ascii_uppercase().replace('_', "-");
        let padded = format!(" \t{upper}\n");
        assert_eq!(
            SectionType::retired_type_of(&padded),
            Some(*retired),
            "{padded:?}"
        );
        assert!(SectionType::is_retired_name(&padded), "{padded:?}");
    }
    for retired in SectionType::RETIRED_NAMES {
        assert_eq!(SectionType::retired_type_of(retired), Some(*retired));
    }
    for name in SectionType::NAMES {
        assert_eq!(SectionType::retired_type_of(name), None, "{name}");
    }
    assert_eq!(SectionType::retired_type_of("example"), None);
    assert_eq!(SectionType::retired_type_of("pitfall"), None);
}

/// The two arms of `SectionType::from_str_loose` that resolved to the retired types,
/// copied verbatim from crates/khive-brain-core/src/section_type.rs at a84eb373, the
/// last revision before the retirement.
const PRE_RETIREMENT_ARMS: &str = r#"
            "references" | "reference" | "bibliography" | "related" | "see_also"
            | "further_reading" | "citations" | "links" => Some(Self::References),
            "other" | "misc" | "miscellaneous" | "notes" | "appendix" => Some(Self::Other),
"#;

#[test]
fn retired_spellings_are_exactly_the_pre_retirement_alias_arms() {
    let mut expected = Vec::new();
    let mut rest = PRE_RETIREMENT_ARMS;
    while let Some((arm, after)) = rest.split_once("=> Some(Self::") {
        let (variant, after) = after.split_once(')').expect("arm ends with a variant");
        let retired = match variant {
            "References" => "references",
            "Other" => "other",
            other => panic!("unexpected variant {other}"),
        };
        for spelling in arm.split('"').skip(1).step_by(2) {
            expected.push((spelling, retired));
        }
        rest = after;
    }
    assert_eq!(expected.len(), 13);
    assert_eq!(SectionType::RETIRED_SPELLINGS, expected.as_slice());
}

#[test]
fn unicode_whitespace_is_trimmed_as_the_alias_table_always_did() {
    for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
        if !c.is_whitespace() {
            continue;
        }
        let example = format!("{c}Example{c}");
        assert_eq!(
            SectionType::from_str_loose(&example),
            Some(SectionType::Examples),
            "{example:?}"
        );
        let notes = format!("{c}Notes{c}");
        assert_eq!(
            SectionType::retired_type_of(&notes),
            Some("other"),
            "{notes:?}"
        );
    }
}
