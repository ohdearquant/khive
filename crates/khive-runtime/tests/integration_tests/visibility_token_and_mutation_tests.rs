// =============================================================================
// Fix 4: visible-set token invariants (primary always included, no duplicates)
// =============================================================================

/// No extra-visible namespaces → visible set contains only the primary.
#[test]
fn mint_with_visibility_empty_extra_yields_primary_only() {
    let rt = rt();
    let tok = rt
        .authorize_with_visibility(Namespace::parse("ns-primary-only").unwrap(), vec![])
        .unwrap();

    let vis = tok.visible_namespaces();
    assert_eq!(vis.len(), 1, "primary only when no extras given");
    assert_eq!(vis[0].as_str(), "ns-primary-only");
    assert_eq!(tok.namespace().as_str(), "ns-primary-only");
}

/// When the primary is repeated in the extra list it must not appear twice.
#[test]
fn mint_with_visibility_deduplicates_primary_in_extras() {
    let rt = rt();
    let tok = rt
        .authorize_with_visibility(
            Namespace::parse("ns-dedup").unwrap(),
            vec![
                Namespace::parse("ns-dedup").unwrap(),
                Namespace::parse("ns-extra").unwrap(),
            ],
        )
        .unwrap();

    let vis = tok.visible_namespaces();
    assert_eq!(vis.len(), 2, "primary counted once, one distinct extra");
    assert_eq!(vis[0].as_str(), "ns-dedup");
    assert_eq!(vis[1].as_str(), "ns-extra");
}

// =============================================================================
// Fix 1: mutations confined to primary namespace; reads use visible set
// =============================================================================

/// A note written into an extra-visible namespace can be read back through
/// the visible-set token (resolve uses the visible set for notes).
#[tokio::test]
async fn resolve_uses_visible_set_for_note_in_extra_namespace() {
    let rt = rt();
    let _tok_a = rt.authorize(Namespace::parse("res-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("res-b").unwrap()).unwrap();

    let note_b = rt
        .create_note(&tok_b, "observation", None, "NoteInB", None, None, vec![])
        .await
        .unwrap();

    // visible-set token: primary=res-a, sees res-b too.
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("res-a").unwrap(),
            vec![Namespace::parse("res-b").unwrap()],
        )
        .unwrap();

    // get_note_including_deleted uses resolve() which should honour visible set.
    let fetched = rt
        .get_note_including_deleted(&vis_tok, note_b.id)
        .await
        .expect("call must not error");
    assert!(
        fetched.is_some(),
        "note in extra-visible namespace must be readable via visible-set token"
    );
    assert_eq!(fetched.unwrap().content, "NoteInB");
}

/// A link whose target lives in the extra-visible (but not primary) namespace
/// must succeed — endpoint existence is a by-ID check, and by-ID ops are
/// namespace-agnostic regardless of the caller's primary/visible-set distinction
/// (ADR-007 Rule 2; #631). This supersedes the prior
/// `link_refuses_target_in_visible_but_not_primary_namespace` expectation, which
/// asserted the pre-#631 bug (endpoint existence gated on `token.namespace()`) as
/// intentional mutation-safety behavior.
#[tokio::test]
async fn link_target_in_visible_but_not_primary_namespace_succeeds() {
    let rt = rt();
    let tok_a = rt
        .authorize(Namespace::parse("link-mut-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("link-mut-b").unwrap())
        .unwrap();

    let entity_a = rt
        .create_entity_with_embedding_report(
            &tok_a,
            "concept",
            None,
            "SrcEntity",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let entity_b = rt
        .create_entity_with_embedding_report(
            &tok_b,
            "concept",
            None,
            "TgtEntity",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // primary=link-mut-a, visible=[link-mut-a, link-mut-b].
    // entity_b lives in link-mut-b (visible, not primary).
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("link-mut-a").unwrap(),
            vec![Namespace::parse("link-mut-b").unwrap()],
        )
        .unwrap();

    let result = rt
        .link(
            &vis_tok,
            entity_a.id,
            entity_b.id,
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await;
    assert!(
        result.is_ok(),
        "link with target in visible-only namespace must succeed (#631), got {result:?}"
    );
}

/// An annotates note whose annotated target lives in the extra-visible (but not
/// primary) namespace must succeed — same by-ID, namespace-agnostic contract as
/// `link` (ADR-007 Rule 2; #631). This supersedes the prior
/// `create_note_annotates_refuses_target_in_visible_only_namespace` expectation.
#[tokio::test]
async fn create_note_annotates_target_in_visible_only_namespace_succeeds() {
    let rt = rt();
    let _tok_a = rt
        .authorize(Namespace::parse("ann-mut-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("ann-mut-b").unwrap())
        .unwrap();

    let entity_b = rt
        .create_entity_with_embedding_report(
            &tok_b,
            "concept",
            None,
            "AnnotTarget",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // primary=ann-mut-a, visible=[ann-mut-a, ann-mut-b].
    // entity_b lives in ann-mut-b (visible, not primary).
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("ann-mut-a").unwrap(),
            vec![Namespace::parse("ann-mut-b").unwrap()],
        )
        .unwrap();

    let result = rt
        .create_note(
            &vis_tok,
            "observation",
            None,
            "AnnotNote",
            None,
            None,
            vec![entity_b.id],
        )
        .await;
    assert!(
        result.is_ok(),
        "annotates with target in visible-only namespace must succeed (#631), got {result:?}"
    );
}
