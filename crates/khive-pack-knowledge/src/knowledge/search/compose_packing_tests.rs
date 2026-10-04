use super::super::compose::{ComposeSectionResult, ScoreBreakdown};
use super::*;

fn atom(number: u128, name: &str, content: String) -> Atom {
    Atom {
        id: Uuid::from_u128(number),
        namespace: "local".into(),
        slug: format!("atom-{number}"),
        name: name.into(),
        content,
        tags: "[]".into(),
        properties: None,
        status: Some("reviewed".into()),
        source_uri: None,
        source_type: None,
        finalized: true,
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    }
}

fn item(atom: &Atom, score: f32) -> ScoredTextItem {
    ScoredTextItem {
        id: atom.id.to_string(),
        slug: atom.slug.clone(),
        name: atom.name.clone(),
        text: atom.content.clone(),
        score,
    }
}

fn domain(name: String) -> Domain {
    Domain {
        id: Uuid::from_u128(999),
        namespace: "local".into(),
        slug: "test-domain".into(),
        name,
        description: None,
        tags: "[]".into(),
        members: "[]".into(),
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    }
}

fn section(atom: &Atom, heading: &str, content: String, score: f32) -> ComposeSectionResult {
    ComposeSectionResult {
        section_id: Uuid::new_v4().to_string(),
        atom_id: atom.id.to_string(),
        section_type: "overview".into(),
        heading: heading.into(),
        content,
        score,
        score_breakdown: ScoreBreakdown {
            section_cosine: score,
            section_bm25: 0.0,
            atom_cosine: score,
            domain_score: 0.0,
            type_weight: 0.0,
        },
    }
}

#[test]
fn actual_markdown_budget_prices_query_domain_and_explain_in_both_modes() {
    let query = "q".repeat(300);
    let domains = vec![domain("long-domain-".repeat(15))];
    let atom = atom(1, "Source Atom", "a".repeat(1_530));
    let atoms = vec![atom.clone()];
    let items = vec![item(&atom, 0.9)];
    let whole = pack_compose_markdown(&query, &domains, &atoms, &items, &[], true, 2_000);
    assert!(whole.markdown.len() <= 2_000);
    assert!(
        whole.included_atom_ids.is_empty(),
        "long atom must not overflow"
    );

    let sections = vec![section(&atom, "Summary", "s".repeat(1_530), 0.9)];
    let sectioned = pack_compose_markdown(&query, &domains, &atoms, &items, &sections, true, 2_000);
    assert!(sectioned.markdown.len() <= 2_000);
    assert!(
        sectioned.sections.is_empty(),
        "long section must not overflow"
    );

    // Even a query longer than the entire budget has a bounded display;
    // the response's structured `query` field still carries the full text.
    let long_query = "é".repeat(1_500);
    let bounded = pack_compose_markdown(&long_query, &domains, &atoms, &items, &[], false, 2_000);
    assert!(bounded.markdown.len() <= 2_000);
    assert!(bounded.markdown.is_char_boundary(bounded.markdown.len()));
}

#[test]
fn atom_only_packing_skips_oversized_first_hit_and_keeps_smaller_hits() {
    let atoms = vec![
        atom(1, "Too Large", "x".repeat(3_000)),
        atom(2, "Small Two", "y".repeat(300)),
        atom(3, "Small Three", "z".repeat(300)),
    ];
    let items: Vec<_> = atoms
        .iter()
        .zip([0.9, 0.8, 0.7])
        .map(|(a, s)| item(a, s))
        .collect();
    let packed = pack_compose_markdown("query", &[], &atoms, &items, &[], false, 2_000);
    assert!(!packed.markdown.contains("Too Large"));
    assert!(packed.markdown.contains("Small Two"));
    assert!(packed.markdown.contains("Small Three"));
    assert_eq!(packed.included_atom_ids.len(), 2);
}

#[test]
fn mixed_section_and_sectionless_atoms_both_survive() {
    let atoms = vec![
        atom(1, "Sectioned", "source body".into()),
        atom(2, "Sectionless", "whole atom body".into()),
    ];
    let items = vec![item(&atoms[0], 0.9), item(&atoms[1], 0.8)];
    let sections = vec![section(&atoms[0], "Overview", "section body".into(), 0.9)];
    let packed = pack_compose_markdown("query", &[], &atoms, &items, &sections, true, 2_000);
    assert!(packed.markdown.contains("section body"));
    assert!(packed.markdown.contains("whole atom body"));
    assert_eq!(packed.sections.len(), 1);
    assert_eq!(packed.included_atom_ids.len(), 2);
}

#[test]
fn kg_tail_skips_oversized_hit_and_prices_heading_exactly() {
    let hits = vec![
        KgEntityHit {
            id: "large".into(),
            kind: "concept".into(),
            name: "large".into(),
            description: "x".repeat(3_000),
            score: 0.9,
        },
        KgEntityHit {
            id: "small".into(),
            kind: "concept".into(),
            name: "small".into(),
            description: "short".into(),
            score: 0.8,
        },
    ];
    let kept = trim_kg_entities_to_budget(hits, 80);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, "small");
    assert!(format_kg_entities_markdown(&kept).len() <= 80);
}
