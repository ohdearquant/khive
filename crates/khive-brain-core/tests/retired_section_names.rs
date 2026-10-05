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
