//! Organization endpoint pairs in the base contract (ADR-002, amended 2026-10-05):
//! `org located_in concept` and the symmetric `org competes_with org`.

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};
use khive_storage::EdgeRelation;
use uuid::Uuid;

async fn entity(rt: &KhiveRuntime, tok: &NamespaceToken, kind: &str, name: &str) -> Uuid {
    rt.create_entity_with_embedding_report(tok, kind, None, name, None, None, vec![])
        .await
        .map(|(entity, _report)| entity.id)
        .unwrap()
}

fn refused_by_allowlist(result: Result<impl std::fmt::Debug, impl std::fmt::Display>) -> bool {
    match result {
        Ok(_) => false,
        Err(error) => error.to_string().contains("base endpoint allowlist"),
    }
}

#[tokio::test]
async fn org_located_in_concept_is_allowed_and_its_neighbours_are_refused() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let company = entity(&rt, &tok, "org", "Example Holdings").await;
    let parent = entity(&rt, &tok, "org", "Example Group").await;
    let jurisdiction = entity(&rt, &tok, "concept", "Example Islands").await;

    let edge = rt
        .link(
            &tok,
            company,
            jurisdiction,
            EdgeRelation::LocatedIn,
            1.0,
            None,
        )
        .await
        .expect("org -> concept located_in is a base row");
    assert_eq!(edge.relation, EdgeRelation::LocatedIn);

    let reversed = rt
        .link(
            &tok,
            jurisdiction,
            company,
            EdgeRelation::LocatedIn,
            1.0,
            None,
        )
        .await;
    assert!(
        refused_by_allowlist(reversed),
        "concept -> org located_in is not a base row"
    );
    let org_to_org = rt
        .link(&tok, company, parent, EdgeRelation::LocatedIn, 1.0, None)
        .await;
    assert!(
        refused_by_allowlist(org_to_org),
        "org -> org located_in is not a base row"
    );
}

#[tokio::test]
async fn org_competes_with_org_is_allowed_and_org_project_is_refused() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let first = entity(&rt, &tok, "org", "First Corp").await;
    let second = entity(&rt, &tok, "org", "Second Corp").await;
    let product = entity(&rt, &tok, "project", "Second Product").await;

    let edge = rt
        .link(&tok, first, second, EdgeRelation::CompetesWith, 0.8, None)
        .await
        .expect("org <-> org competes_with is a base row");
    assert_eq!(edge.relation, EdgeRelation::CompetesWith);

    let mixed = rt
        .link(&tok, first, product, EdgeRelation::CompetesWith, 0.8, None)
        .await;
    assert!(
        refused_by_allowlist(mixed),
        "org <-> project competes_with is not a base row"
    );
}
