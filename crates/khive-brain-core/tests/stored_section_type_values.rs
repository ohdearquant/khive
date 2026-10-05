//! Every distinct `section_type` value found in a populated knowledge store reads as
//! exactly one type: a current type through the heading alias table, or a retired type
//! through its retired spelling. A stored value that reads as neither makes its whole
//! atom unreadable, so this list is the set the parser has to cover.

use khive_brain_core::SectionType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    Current(SectionType),
    Retired(&'static str),
}

/// The distinct stored values of a store written by bulk import before the
/// `references` / `other` retirement, with the type each one must read as.
const STORED_VALUES: [(&str, Reads); 15] = [
    ("other", Reads::Retired("other")),
    ("example", Reads::Current(SectionType::Examples)),
    ("overview", Reads::Current(SectionType::Overview)),
    ("algorithm", Reads::Current(SectionType::Formalism)),
    ("reference", Reads::Retired("references")),
    ("pitfall", Reads::Current(SectionType::FailureModes)),
    ("definition", Reads::Current(SectionType::CoreModel)),
    ("comparison", Reads::Current(SectionType::ExpertLens)),
    ("examples", Reads::Current(SectionType::Examples)),
    ("core_model", Reads::Current(SectionType::CoreModel)),
    ("failure_modes", Reads::Current(SectionType::FailureModes)),
    ("formalism", Reads::Current(SectionType::Formalism)),
    ("expert_lens", Reads::Current(SectionType::ExpertLens)),
    (
        "operational_guidance",
        Reads::Current(SectionType::OperationalGuidance),
    ),
    ("motivation", Reads::Current(SectionType::Overview)),
];

fn reads(value: &str) -> Option<Reads> {
    let current = SectionType::from_str_loose(value);
    let retired = SectionType::retired_type_of(value);
    match (current, retired) {
        (Some(section), None) => Some(Reads::Current(section)),
        (None, Some(name)) => Some(Reads::Retired(name)),
        (Some(section), Some(name)) => {
            panic!("{value:?} reads as both {section} and retired {name}")
        }
        (None, None) => None,
    }
}

#[test]
fn every_stored_section_type_value_reads_as_exactly_one_type() {
    for (value, expected) in STORED_VALUES {
        assert_eq!(reads(value), Some(expected), "stored value {value:?}");
    }
}

#[test]
fn the_stored_values_are_distinct() {
    let mut values: Vec<&str> = STORED_VALUES.iter().map(|(value, _)| *value).collect();
    values.sort_unstable();
    values.dedup();
    assert_eq!(values.len(), STORED_VALUES.len());
}

#[test]
fn a_value_outside_both_tables_still_reads_as_nothing() {
    // The table covers the stored set; it is not a catch-all.
    for value in ["glossary", "summary", "", "pitfal"] {
        assert_eq!(reads(value), None, "{value:?}");
    }
}
