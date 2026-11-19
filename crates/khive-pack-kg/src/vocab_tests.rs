use super::*;
use std::str::FromStr;

#[test]
fn note_kind_roundtrip() {
    for kind in NoteKind::ALL {
        let parsed = NoteKind::from_str(kind.name()).unwrap();
        assert_eq!(parsed, kind);
    }
}

#[test]
fn note_kind_aliases_rejected() {
    // Only canonical names are accepted; aliases are rejected.
    assert!(NoteKind::from_str("obs").is_err());
    assert!(NoteKind::from_str("finding").is_err());
    assert!(NoteKind::from_str("q").is_err());
    assert!(NoteKind::from_str("choice").is_err());
    assert!(NoteKind::from_str("ref").is_err());
    assert!(NoteKind::from_str("citation").is_err());
}

#[test]
fn entity_kind_roundtrip_all() {
    for kind in EntityKind::ALL {
        let parsed = EntityKind::from_str(kind.name()).unwrap();
        assert_eq!(parsed, kind);
    }
}

#[test]
fn entity_kind_resource_aliases() {
    // `resource` is the 9th entity kind; these are its accepted aliases.
    for alias in ["atom", "runbook", "template", "prompt", "skill", "tool"] {
        let parsed = EntityKind::from_str(alias).unwrap();
        assert_eq!(
            parsed,
            EntityKind::Resource,
            "alias {alias:?} must map to Resource"
        );
    }
}

#[test]
fn entity_kind_names_length() {
    assert_eq!(EntityKind::ALL.len(), EntityKind::NAMES.len());
    assert_eq!(EntityKind::ALL.len(), 9);
}
