use std::collections::BTreeSet;

use super::resource_alias_for_replay;

fn quoted_aliases(pattern: &str) -> BTreeSet<String> {
    pattern
        .split('|')
        .map(|literal| {
            serde_json::from_str::<String>(literal.trim())
                .unwrap_or_else(|error| panic!("expected a quoted alias in {pattern:?}: {error}"))
        })
        .collect()
}

fn kg_resource_aliases() -> BTreeSet<String> {
    // Like kg_bulk_entry_field_names in tests/create_validation.rs, read the
    // crate-private KG declaration directly rather than copy its vocabulary.
    let source = include_str!("../../khive-pack-kg/src/vocab.rs");
    let from_str = source
        .split_once("impl std::str::FromStr for EntityKind {")
        .expect("the KG entity-kind FromStr implementation must exist")
        .1
        .split_once("\n}")
        .expect("the KG entity-kind FromStr implementation must close")
        .0;
    let resource_arm = from_str
        .split_once("Ok(Self::Resource)")
        .expect("the KG resource alias arm must exist")
        .0
        .rsplit_once("=>")
        .expect("the KG resource alias arm must have a pattern")
        .0
        .rsplit_once(',')
        .expect("the KG resource alias arm must follow another match arm")
        .1;
    quoted_aliases(resource_arm)
}

fn schedule_resource_aliases() -> BTreeSet<String> {
    let source = include_str!("handlers.rs");
    let body = source
        .split_once("fn resource_alias_for_replay(normalized: &str) -> bool {")
        .expect("the schedule resource alias function must exist")
        .1
        .split_once("\n}")
        .expect("the schedule resource alias function must close")
        .0;
    let pattern = body
        .split_once("matches!(")
        .expect("the schedule resource aliases must use a literal matches! pattern")
        .1
        .split_once("normalized,")
        .expect("the schedule resource aliases must match the normalized value")
        .1
        .trim()
        .strip_suffix(')')
        .expect("the schedule resource alias matches! expression must close");
    quoted_aliases(pattern)
}

#[test]
fn entity_kind_resource_aliases_match_real_vocab() {
    let kg_aliases = kg_resource_aliases();
    let schedule_aliases = schedule_resource_aliases();
    assert!(
        kg_aliases.len() > 1 && kg_aliases.contains("resource"),
        "the actual KG resource arm must supply its canonical name and aliases: {kg_aliases:?}"
    );
    assert_eq!(
        schedule_aliases, kg_aliases,
        "schedule's resource aliases must equal the actual KG FromStr arm"
    );
    for alias in &kg_aliases {
        assert!(
            resource_alias_for_replay(alias),
            "the schedule predicate must accept source-declared alias {alias:?}"
        );
    }
    assert!(
        !resource_alias_for_replay("not-a-resource-alias"),
        "the schedule predicate must reject an undeclared spelling"
    );
}
