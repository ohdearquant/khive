use serde_json::{json, Value};

use super::*;
use crate::definition::{OperationType, Slot, SlotRelation};

fn slot(name: &str, kind: &str, role: &str, qualifier: Option<&str>, repeats: bool) -> Slot {
    Slot {
        name: name.to_owned(),
        repeats,
        required: true,
        kind: Some(kind.to_owned()),
        role: Some(role.to_owned()),
        qualifier: qualifier.map(str::to_owned),
        defined_by: None,
    }
}

fn related(name: &str, relation: &str, to: &str) -> Slot {
    Slot {
        name: name.to_owned(),
        repeats: false,
        required: true,
        kind: None,
        role: None,
        qualifier: None,
        defined_by: Some(SlotRelation {
            relation: relation.to_owned(),
            slot: to.to_owned(),
        }),
    }
}

fn type_of(slots: Vec<Slot>) -> OperationType {
    OperationType {
        kind: "operation_type".to_owned(),
        id: "t".to_owned(),
        version: 1,
        slots,
        steps: Vec::new(),
        rules: Vec::new(),
    }
}

fn terms() -> Terms {
    Terms::from_reply(&json!({"terms": {
        "kinds": [{"name": "person", "aliases": []}, {"name": "parcel", "aliases": []}, {"name": "address", "aliases": []}],
        "roles": [{"name": "applicant", "aliases": ["petitioner"]}, {"name": "parcel", "aliases": []}, {"name": "owner", "aliases": []}, {"name": "mailing address", "aliases": []}],
        "relations": [{"name": "owns", "aliases": [], "carries": false}, {"name": "holds", "aliases": [], "carries": false}, {"name": "belongs to", "aliases": [], "carries": true}],
        "counting_qualifiers": [{"name": "second", "aliases": ["2nd"]}]
    }}))
}

fn subject(view: Value) -> Subject {
    Subject::from_view(&view).expect("subject view")
}

fn by(resolution: &Resolution) -> Vec<(String, Value, Value)> {
    resolution
        .subjects
        .iter()
        .map(|v| {
            (
                v["subject"].as_str().unwrap_or_default().to_owned(),
                v["resolved_by"].clone(),
                v["slot"].clone(),
            )
        })
        .collect()
}

#[test]
fn two_subjects_on_a_slot_that_does_not_repeat_are_undecided_and_on_one_that_repeats_resolve() {
    let subjects = [
        subject(json!({"id": "a", "kind": "person", "role": "applicant"})),
        subject(json!({"id": "b", "kind": "person", "role": "Petitioner"})),
    ];
    let one = resolve(
        &type_of(vec![slot("applicant", "person", "applicant", None, false)]),
        &terms(),
        &subjects,
        &[],
    );
    for (name, resolved_by, slot) in by(&one) {
        assert_eq!(resolved_by, "undecided", "{name}");
        assert_eq!(slot, Value::Null, "{name}");
    }
    assert_eq!(one.subjects[0]["candidates"], json!(["applicant"]));

    let many = resolve(
        &type_of(vec![slot("applicant", "person", "applicant", None, true)]),
        &terms(),
        &subjects,
        &[],
    );
    for (name, resolved_by, slot) in by(&many) {
        assert_eq!(resolved_by, "role", "{name}");
        assert_eq!(slot, "applicant", "{name}");
    }
}

#[test]
fn a_counting_qualifier_falls_to_the_unqualified_slot_and_any_other_qualifier_blocks() {
    let type_ = type_of(vec![
        slot("applicant", "person", "applicant", None, false),
        slot(
            "co_applicant",
            "person",
            "applicant",
            Some("additional"),
            false,
        ),
    ]);
    let subjects = [
        subject(
            json!({"id": "counted", "kind": "person", "role": "applicant", "qualifier": "2nd"}),
        ),
        subject(
            json!({"id": "extra", "kind": "person", "role": "applicant", "qualifier": "  Additional "}),
        ),
        subject(
            json!({"id": "former", "kind": "person", "role": "applicant", "qualifier": "former"}),
        ),
    ];
    let resolution = resolve(&type_, &terms(), &subjects, &[]);
    assert_eq!(
        by(&resolution),
        vec![
            ("counted".to_owned(), json!("role"), json!("applicant")),
            ("extra".to_owned(), json!("role"), json!("co_applicant")),
            ("former".to_owned(), json!("undecided"), Value::Null),
        ]
    );
    assert_eq!(
        resolution.subjects[2]["candidates"],
        json!(["applicant", "co_applicant"])
    );
}

#[test]
fn a_subject_with_two_relations_to_two_slots_is_undecided() {
    let type_ = type_of(vec![
        slot("parcel", "parcel", "parcel", None, false),
        related("parcel_owner", "owns", "parcel"),
        related("parcel_holder", "holds", "parcel"),
    ]);
    let subjects = [
        subject(json!({"id": "parcel", "kind": "parcel", "role": "parcel"})),
        subject(
            json!({"id": "someone", "kind": "person", "role": "owner", "relations": [
                {"relation": "owns", "to": "parcel"},
                {"relation": "holds", "to": "Parcel"}
            ]}),
        ),
    ];
    let resolution = resolve(&type_, &terms(), &subjects, &[]);
    assert_eq!(resolution.subjects[1]["resolved_by"], "undecided");
    assert_eq!(
        resolution.subjects[1]["candidates"],
        json!(["parcel_holder", "parcel_owner"])
    );
}

#[test]
fn a_chain_of_carrying_relations_goes_with_the_slot_at_its_end() {
    let type_ = type_of(vec![slot("applicant", "person", "applicant", None, false)]);
    let subjects = [
        subject(
            json!({"id": "zip", "kind": "address", "role": "mailing address", "relations": [{"relation": "belongs to", "to": "address"}]}),
        ),
        subject(
            json!({"id": "address", "kind": "address", "role": "mailing address", "relations": [{"relation": "Belongs  To", "to": "applicant"}]}),
        ),
        subject(json!({"id": "applicant", "kind": "person", "role": "applicant"})),
    ];
    let resolution = resolve(&type_, &terms(), &subjects, &[]);
    assert_eq!(
        by(&resolution),
        vec![
            ("zip".to_owned(), json!("carried"), json!("applicant")),
            ("address".to_owned(), json!("carried"), json!("applicant")),
            ("applicant".to_owned(), json!("role"), json!("applicant")),
        ]
    );
    assert_eq!(resolution.subjects[0]["carried_by"], "address");
    assert_eq!(resolution.subjects[1]["carried_by"], "applicant");
}

#[test]
fn the_newest_decision_naming_a_subject_decides_it_and_an_older_one_still_decides_the_rest() {
    let type_ = type_of(vec![
        slot("applicant", "person", "applicant", None, true),
        slot("parcel", "parcel", "parcel", None, false),
    ]);
    let subjects = [
        subject(json!({"id": "x", "kind": "person", "role": "owner"})),
        subject(json!({"id": "y", "kind": "person", "role": "owner"})),
    ];
    let decisions = [
        Decision {
            id: "d1".to_owned(),
            subjects: vec!["x".to_owned(), "y".to_owned()],
            slot: Some("applicant".to_owned()),
        },
        Decision {
            id: "d2".to_owned(),
            subjects: vec!["Y".to_owned(), "gone".to_owned()],
            slot: None,
        },
    ];
    let resolution = resolve(&type_, &terms(), &subjects, &decisions);
    assert_eq!(resolution.subjects[0]["decision"], "d1");
    assert_eq!(resolution.subjects[0]["slot"], "applicant");
    assert_eq!(resolution.subjects[1]["decision"], "d2");
    assert_eq!(resolution.subjects[1]["not_this_type"], true);
    assert_eq!(
        resolution.lapsed,
        vec![json!({"decision": "d2", "subject": "gone"})]
    );
}
