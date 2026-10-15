use super::*;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_types::EndpointKind;

#[tokio::test]
async fn kg_pack_registers_all_canonical_note_kinds() {
    assert_eq!(KgPack::NOTE_KINDS, crate::vocab::NoteKind::NAMES);

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    let registry = builder.build().expect("registry build");
    let kinds = registry.all_note_kinds();

    assert_eq!(kinds, crate::vocab::NoteKind::NAMES);
    assert!(kinds.contains(&"head"));
}

fn endpoint_str(ep: &EndpointKind) -> &'static str {
    match ep {
        EndpointKind::EntityOfKind(k) => k,
        EndpointKind::NoteOfKind(k) => k,
        EndpointKind::EntityOfType { kind, .. } => kind,
    }
}

/// ADR-076 §D2: no duplicate (relation, source, target) triples in KG_EDGE_RULES.
#[test]
fn kg_pack_edge_rules_contain_no_duplicate_triples() {
    let triples: Vec<(&str, &str, &str)> = KG_EDGE_RULES
        .iter()
        .map(|r| {
            (
                r.relation.as_str(),
                endpoint_str(&r.source),
                endpoint_str(&r.target),
            )
        })
        .collect();

    let mut seen: Vec<(&str, &str, &str)> = triples.clone();
    seen.sort_unstable();

    let mut duplicates: Vec<(&str, &str, &str)> = vec![];
    for i in 1..seen.len() {
        if seen[i] == seen[i - 1] {
            duplicates.push(seen[i]);
        }
    }

    assert!(
        duplicates.is_empty(),
        "KG_EDGE_RULES contains duplicate triples: {duplicates:?}; \
         remove the redundant entries"
    );
}

/// Deliberate-change tripwire: KG_EDGE_RULES covers exactly this relation set.
#[test]
fn kg_pack_edge_rules_cover_expected_relations() {
    let mut seen: Vec<&str> = KG_EDGE_RULES.iter().map(|r| r.relation.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();

    let expected = &[
        "contains",
        "depends_on",
        "enables",
        "instance_of",
        "part_of",
        "precedes",
    ];

    assert_eq!(
        seen, *expected,
        "KG_EDGE_RULES covers a different relation set than expected; \
         update this test if the change is intentional"
    );
}
