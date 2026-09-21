//! Black-box vocabulary checks against the crate's public surface (ADR-191
//! D2/D3): exactly five verbs, exactly two pack-declared `EDGE_RULES` rows,
//! and the closed `kinds` set `web.extract` documents in its own `ParamDef`.

use khive_pack_web::WebPack;
use khive_types::Pack;

#[test]
fn exactly_five_verbs_named_per_d3() {
    let names: Vec<&str> = <WebPack as Pack>::HANDLERS.iter().map(|h| h.name).collect();
    assert_eq!(
        names,
        vec![
            "web.fetch",
            "web.extract",
            "web.ingest",
            "web.search",
            "web.refresh"
        ],
        "D3's operations table names exactly these five verbs, in this order of introduction"
    );
}

#[test]
fn exactly_two_edge_rules_site_contains_page_and_resource() {
    let rules = <WebPack as Pack>::EDGE_RULES;
    assert_eq!(
        rules.len(),
        2,
        "D2: every other row this pack's operations produce is legal under the base \
         contract already — links_to, derived_from, supersedes, annotates need no \
         pack-declared row"
    );
    for rule in rules {
        assert_eq!(rule.relation, khive_types::EdgeRelation::Contains);
    }
}

#[test]
fn pack_declares_no_entity_or_note_kinds_of_its_own() {
    // ADR-191 D1: site/page/resource are khive-types entity_type SUBTYPES,
    // not pack-owned ENTITY_KINDS — the web pack introduces no new base
    // entity kind or note kind.
    assert!(<WebPack as Pack>::ENTITY_KINDS.is_empty());
    assert!(<WebPack as Pack>::NOTE_KINDS.is_empty());
}

#[test]
fn web_extract_kinds_param_names_the_closed_set() {
    let extract = <WebPack as Pack>::HANDLERS
        .iter()
        .find(|h| h.name == "web.extract")
        .expect("web.extract is registered");
    let kinds_param = extract
        .params
        .iter()
        .find(|p| p.name == "kinds")
        .expect("web.extract documents a kinds parameter");
    for expected in ["text", "links", "sitemap", "feed"] {
        assert!(
            kinds_param.description.contains(expected),
            "kinds description should name {expected:?}: {:?}",
            kinds_param.description
        );
    }
}
