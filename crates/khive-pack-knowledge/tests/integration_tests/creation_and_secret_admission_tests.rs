// ── KPK-002: DomainInput deny_unknown_fields + domain-mirror content-word minimum ──

#[tokio::test]
async fn kpk002_domain_input_rejects_unknown_fields() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "test-domain",
                    "name": "Test Domain",
                    "description": "A domain with enough words to pass the twenty word minimum content requirement for testing.",
                    "unknown_field_xyz": "should cause rejection"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "unknown field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_mirror_atom_below_word_minimum_is_rejected() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "sparse-domain",
                    "name": "Sparse Domain",
                    "description": "Too short"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("20") || msg.contains("words") || msg.contains("content"),
        "description below 20-word minimum must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_mirror_atom_empty_description_is_rejected() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "empty-desc-domain",
                    "name": "Empty Desc Domain"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("20") || msg.contains("words") || msg.contains("content"),
        "missing description must be rejected as below 20-word minimum; got: {msg}"
    );
}

#[tokio::test]
async fn kpk002_domain_with_sufficient_description_is_accepted() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "rich-domain",
                    "name": "Rich Domain",
                    "description": "This domain covers retrieval augmented generation patterns for building scalable knowledge systems with structured graph storage and semantic search capabilities for AI agents.",
                    "tags": ["rag", "retrieval"],
                    "members": []
                }]
            }),
        )
        .await
        .expect("domain with sufficient description must be accepted");
    assert_eq!(resp["created"], json!(1u64));
    assert_eq!(resp["updated"], json!(0u64));
}

// ── Secret-gate regression tests ─────────────────────────────────────────────

fn is_secret_detected(err: &RuntimeError) -> bool {
    matches!(err.refusal_source(), RuntimeError::SecretDetected(_))
}

/// A batch refusal names the atom it came from, by position. Without this the
/// caller holds N atoms and one error describing text that lives in whichever
/// sibling refused, so a clean atom and a refusing one are indistinguishable in
/// the response (#2605). The location is positional because the slug is itself a
/// scanned field: echoing it would return the very text the gate refused.
#[tokio::test]
async fn upsert_atoms_refusal_names_the_offending_atom_not_just_the_text() {
    let f = pack(rt());
    let clean_atom = json!({
        "slug": "clean-sibling-atom",
        "name": "Clean Sibling",
        "content": "This atom carries ordinary prose about retrieval augmented generation and contains nothing credential shaped anywhere in it, only plain words about ranking and recall quality.",
    });
    let result = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    clean_atom.clone(),
                    {
                        "slug": "atom-carrying-the-credential",
                        "name": "Offending Atom",
                        "content": "The deploy token for the staging cluster is ghp_FakeGitHubToken0000000000000000000 and it must be rotated before the next release ships to every tenant.", // gitleaks:allow
                    },
                ]
            }),
        )
        .await;
    let err = result.expect_err("a credential in the second atom must refuse the call");
    assert!(
        is_secret_detected(&err),
        "the refusal must stay classified as a secret detection; got: {err:?}"
    );
    let RuntimeError::SecretDetected(matched) = err.refusal_source() else {
        unreachable!("typed secret refusal asserted above");
    };
    assert_eq!(matched.location.as_deref(), Some("atoms[1].content"));
    let rendered = err.to_string();
    assert!(
        rendered.contains("in atoms[1].content"),
        "the refusal must locate the atom and field that produced it; got: {rendered}"
    );
    assert!(
        !rendered.contains("atoms[0]"),
        "the refusal must not point at a bystander atom; got: {rendered}"
    );
    assert!(
        !rendered.contains("atom-carrying-the-credential")
            && !rendered.contains("clean-sibling-atom"),
        "the location is positional: a slug is itself a scanned field and never echoed; got: {rendered}"
    );

    let after_refusal = f
        .dispatch("knowledge.list", json!({"type": "atom"}))
        .await
        .expect("list after secret preflight refusal");
    assert!(
        after_refusal["results"].as_array().expect("atom rows").is_empty(),
        "the whole-batch refusal must not commit the clean sibling or the refusing atom: {after_refusal}"
    );

    let accepted = f
        .dispatch("knowledge.upsert_atoms", json!({"atoms": [clean_atom]}))
        .await
        .expect("the identical clean sibling must succeed when submitted alone");
    assert_eq!(accepted, json!({"created": 1, "updated": 0, "total": 1}));
    let after_retry = f
        .dispatch("knowledge.list", json!({"type": "atom"}))
        .await
        .expect("list separately accepted clean atom");
    let atoms = after_retry["results"].as_array().expect("atom rows");
    assert_eq!(atoms.len(), 1);
    assert_eq!(atoms[0]["slug"], "clean-sibling-atom");
}

/// knowledge.upsert_domains with a credential-shaped slug must be rejected.
#[tokio::test]
async fn upsert_domains_blocks_secret_in_slug_insert() {
    let f = pack(rt());
    let result = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "ghp_FakeGitHubToken0000000000000000000", // gitleaks:allow
                    "name": "Secret Slug Domain",
                    "description": "This domain describes retrieval augmented generation patterns for building scalable AI knowledge systems with structured graph storage and semantic search capabilities.",
                }]
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "upsert_domains with secret in slug must be rejected; got: {result:?}"
    );
}

/// knowledge.upsert_domains with a clean slug must succeed.
#[tokio::test]
async fn upsert_domains_clean_slug_passes() {
    let f = pack(rt());
    let result = f
        .dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "clean-domain-slug",
                    "name": "Clean Domain",
                    "description": "This domain covers retrieval augmented generation patterns for AI knowledge systems at scale with hybrid search and graph traversal features.",
                }]
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "upsert_domains with clean slug must succeed; got: {result:?}"
    );
}
