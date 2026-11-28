use std::collections::HashSet;

use khive_types::{Pack, VerbCategory};

use super::*;

#[test]
fn every_verb_is_namespaced_and_unique() {
    let mut seen = HashSet::new();
    for handler in FormsPack::HANDLERS {
        assert!(handler.name.starts_with("forms."), "{}", handler.name);
        assert!(seen.insert(handler.name), "{} twice", handler.name);
        assert!(!handler.description.is_empty(), "{}", handler.name);
    }
    assert_eq!(seen.len(), 12);
}

#[test]
fn writes_are_commissive_and_reads_assertive() {
    let writes = [
        "forms.load",
        "forms.add_item",
        "forms.same",
        "forms.split",
        "forms.replay",
    ];
    for handler in FormsPack::HANDLERS {
        let expected = if writes.contains(&handler.name) {
            VerbCategory::Commissive
        } else {
            VerbCategory::Assertive
        };
        assert_eq!(handler.category, expected, "{}", handler.name);
    }
}

#[test]
fn the_pack_declares_its_types_and_edges_and_needs_kg() {
    assert_eq!(FormsPack::NAME, "forms");
    assert_eq!(FormsPack::ENTITY_TYPES.len(), 9);
    assert_eq!(FormsPack::EDGE_RULES.len(), 22);
    assert_eq!(FormsPack::REQUIRES, ["kg"]);
}
